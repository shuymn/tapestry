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
    DecodeFile, KeptSounds, LinkCount, OneFile, PickDecode, Recovery, RecoveryFs, Refused,
    Resolution, Resolved, ScanCap, ScanLimits, SetPick, SetRole, Source, Unavailable, Why, choose,
    decode_pick, random_index, resolve, scan,
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

/// The hard links the tests made, as pairs of canonical names. The
/// standard library reads a file's count of names only on Unix, and `game`
/// holds no Windows code, so on Windows the count is this record's (the
/// player's own count is tested in `player`).
#[cfg(windows)]
static LINKED: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

/// The count of names of `path`'s file: the system's on Unix; on Windows 1
/// plus the hard links [`hard_link`] recorded with it.
fn links(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(path).ok().map(|m| m.nlink())
    }
    #[cfg(windows)]
    {
        let path = path.canonicalize().ok()?;
        let linked = LINKED.lock().unwrap();
        let others = linked.iter().filter(|(a, b)| *a == path || *b == path);
        Some(1 + u64::try_from(others.count()).unwrap())
    }
}

/// The probe every test reads identities with.
const LINKS: LinkCount = links;

/// Makes `link` a second name of `original`'s file.
fn hard_link(original: &Path, link: &Path) {
    fs::hard_link(original, link).unwrap();
    #[cfg(windows)]
    LINKED.lock().unwrap().push((canon(original), canon(link)));
}

/// Renames `from` over `to`, ending any hard link `to` was a name of.
fn rename_over(from: &Path, to: &Path) {
    #[cfg(windows)]
    {
        let to = canon(to);
        LINKED.lock().unwrap().retain(|(a, b)| *a != to && *b != to);
    }
    fs::rename(from, to).unwrap();
}

/// The disk with [`LINKS`]: every test opens the recovery state through
/// it, so a pick's identities count names as [`resolve`]'s here do.
#[derive(Debug)]
struct DiskFs(game::sound_sets::DiskFs);

impl Default for DiskFs {
    fn default() -> Self {
        Self(game::sound_sets::DiskFs::new(LINKS))
    }
}

impl RecoveryFs for DiskFs {
    fn lock(&mut self, path: &Path) -> io::Result<bool> {
        self.0.lock(path)
    }

    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        self.0.read(path)
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.0.write(path, bytes)
    }

    fn links(&self) -> LinkCount {
        self.0.links()
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

/// ST2: the default caps are depth 8, 4 096 folders and 65 536 entries.
/// Each cap, set small, at its bound: a set at the deepest depth is found
/// and one deeper is not, while a set listed after that deep branch is
/// still found (the depth cap prunes, it does not stop the scan); the last
/// folder allowed is listed and the next is not; the last entry allowed is
/// listed and the next is not, so a folder whose listing it cuts is not a
/// set. Each cap is named once.
#[test]
fn scan_caps_stop_at_their_bounds() {
    let defaults = ScanLimits::default();
    assert_eq!(
        (defaults.depth, defaults.folders, defaults.entries),
        (8, 4_096, 65_536)
    );

    let base = tmp();
    let limits = ScanLimits {
        depth: 2,
        ..defaults
    };
    let root = base.path().join("deep");
    let two = Path::new("d1/d2");
    put(&root.join(two), "clear.wav");
    put(&root.join(two).join("d3"), "clear.wav");
    put(&root.join(two).join("e3"), "clear.wav");
    put(&root.join("z"), "clear.wav");
    let scanned = scan(&root, SetRole::Sound, &limits);
    assert_eq!(
        scanned.sets,
        [canon(&root.join(two)), canon(&root.join("z"))]
    );
    assert_eq!(scanned.caps, [ScanCap::Depth]);

    // The root and 4 folders under a cap of 4: the root is listed first, so
    // the last folder in order is the 5th.
    let limits = ScanLimits {
        folders: 4,
        ..defaults
    };
    let root = base.path().join("wide");
    for i in 0..4 {
        fs::create_dir_all(root.join(format!("f{i}"))).unwrap();
    }
    for i in [0, 2, 3] {
        put(&root.join(format!("f{i}")), "clear.wav");
    }
    let scanned = scan(&root, SetRole::Sound, &limits);
    assert_eq!(
        scanned.sets,
        [canon(&root.join("f0")), canon(&root.join("f2"))]
    );
    assert_eq!(scanned.caps, [ScanCap::Folders]);

    // Under a cap of 8 entries, the root lists 5 files and the folders `a`
    // and `b`; `a` lists its marker, the 8th entry, and `b`'s marker is the
    // 9th.
    let limits = ScanLimits {
        entries: 8,
        ..defaults
    };
    let root = base.path().join("many");
    fs::create_dir_all(&root).unwrap();
    for i in 0..5 {
        fs::File::create(root.join(format!("n{i}"))).unwrap();
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
    let r = resolve(
        &pick(Some(&sound), Some(&bgm), &defaults),
        LINKS,
        &mut |_| false,
    );
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
    let r = resolve(&pick(None, None, &only), LINKS, &mut |_| false);
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
        let r = resolve(&pick(Some(&four), None, &only), LINKS, &mut |_| false);
        assert_eq!(
            found(&r, SystemKind::Scratch),
            (
                canon(&four.join(format!("scratch.{ext}"))),
                Source::SoundSet
            )
        );
        fs::remove_file(four.join(format!("scratch.{ext}"))).unwrap();
        let r = resolve(&pick(Some(&four), None, &only), LINKS, &mut |_| false);
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
        let _ = tx.send(resolve(&p, LINKS, &mut |_| false));
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

    fn links(&self) -> LinkCount {
        self.disk.links()
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
    resolve(&set_pick(f), LINKS, &mut |_| false).sources[0]
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
    let marker = resolve(&defaults, LINKS, &mut |_| false).sources[2]
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
        let chosen = choose(&sets, &mut recovery, &[], &mut |n| {
            assert_eq!(n, 1);
            at
        });
        assert_eq!(chosen, Some(canon(&other)));
    }
    let chosen = choose(&sets, &mut recovery, &[identity_of_set(&other)], &mut |_| 0);
    assert_eq!(chosen, None, "a skipped set is chosen");
    assert!(random_index(3) < 3);
}

/// The real decode, except that at the file `at` it copies the recovery
/// state as it stands: what a launch that crashed there would leave.
struct CrashAt {
    at: &'static str,
    state: PathBuf,
    left: Mutex<Option<Vec<u8>>>,
}

impl DecodeFile for CrashAt {
    fn decode(
        &self,
        path: &Path,
        rate: u32,
        budget: &PickBudget,
        limits: &BankLimits,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<Decoded, FileRefusal> {
        if path.file_name().is_some_and(|n| n == self.at) {
            *self.left.lock().unwrap() = Some(fs::read(&self.state).unwrap());
        }
        OneFile.decode(path, rate, budget, limits, cancel)
    }
}

/// One folder as the sound root, the BGM root and `defaultsound\`: a launch
/// that crashes decoding its BGM file quarantines the folder for every
/// role, and it stays quarantined across launches while each role chooses
/// again; a healthy sound set beside it is still chosen.
// Covers: T18
#[test]
fn a_folder_quarantined_in_one_role_stays_so_in_every_role() {
    let base = tmp();
    let data = DataRoot::new(base.path().join("data"));
    let shared = data.default_sounds();
    put_bytes(&shared, "clear.wav", &wav(480, 200));
    put_bytes(&shared, "select.wav", &wav(240, -200));
    let other = shared.join("other");
    put_bytes(&other, "clear.wav", &wav(120, 50));
    let both = pick(Some(&shared), Some(&shared), &shared);

    // The first launch crashes decoding `select.wav`, its BGM marker
    // committed.
    let crash = CrashAt {
        at: "select.wav",
        state: data.sound_sets(),
        left: Mutex::new(None),
    };
    {
        let mut recovery = Recovery::open(&data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
        decode_pick(
            &both,
            &KeptSounds::default(),
            &budget,
            &mut recovery,
            &|| false,
            &crash,
        );
    }
    let left = crash
        .left
        .lock()
        .unwrap()
        .take()
        .expect("select.wav decoded");
    fs::write(data.sound_sets(), &left).unwrap();

    let limits = ScanLimits::default();
    let sound_sets = scan(&shared, SetRole::Sound, &limits).sets;
    let bgm_sets = scan(&shared, SetRole::Bgm, &limits).sets;
    assert_eq!(sound_sets, [canon(&shared), canon(&other)]);
    assert_eq!(bgm_sets, [canon(&shared)]);
    for launch in 2..=3 {
        let opened = Recovery::open(&data, Box::new(DiskFs::default())).unwrap();
        let mut recovery = opened.recovery;
        if launch == 2 {
            assert_eq!(
                opened.quarantined.as_ref().map(|q| &q.path),
                Some(&canon(&shared))
            );
        }
        assert_eq!(recovery.quarantined().len(), 1, "launch {launch}");
        let sound = choose(&sound_sets, &mut recovery, &[], &mut |n| {
            assert_eq!(n, 1, "launch {launch}: the sound sets open");
            0
        });
        assert_eq!(sound, Some(canon(&other)), "launch {launch}");
        let bgm = choose(&bgm_sets, &mut recovery, &[], &mut |_| 0);
        assert_eq!(bgm, None, "launch {launch}: the BGM set chosen");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
        let report = decode_pick(
            &pick(sound.as_deref(), bgm.as_deref(), &shared),
            &KeptSounds::default(),
            &budget,
            &mut recovery,
            &|| false,
            &LoggedDecode(Arc::clone(&seen)),
        );
        assert_eq!(
            ops(&seen),
            ["decode clear.wav"],
            "launch {launch}: only the other set is read"
        );
        assert!(matches!(report.decode, PickDecode::Ready(_)));
        assert_eq!(
            recovery.quarantined().len(),
            1,
            "launch {launch}: the quarantine dropped"
        );
    }
}

/// A set whose candidates link out of its folder stays quarantined while
/// the links' targets change: its identity stamps what the folder holds,
/// not what lies outside it, so a set cannot expire its own quarantine
/// through a folder or file that changes between launches.
// Covers: T18
#[test]
fn a_link_out_of_the_set_cannot_expire_its_quarantine() {
    let f = fixture();
    let outside = tmp();
    let folder = outside.path().join("folder");
    put(&folder, "old.wav");
    fs::remove_file(f.set.join("fail.wav")).unwrap();
    link_dir(&f.set.join("fail.wav"), &folder);
    let file = put(outside.path(), "file.wav");
    let file_linked = link_file(&f.set.join("scratch.wav"), &file).is_some();

    let identity = identity_of(&f);
    {
        let mut r = Recovery::open(&f.data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        r.mark(&identity).unwrap();
    }
    let opened = Recovery::open(&f.data, Box::new(DiskFs::default())).unwrap();
    assert_eq!(opened.quarantined, Some(identity));
    let mut recovery = opened.recovery;

    // The targets change between launches: the folder gains a file and
    // the file grows.
    put(&folder, "new.wav");
    if file_linked {
        put_bytes(outside.path(), "file.wav", &wav(500, 7));
    }
    let seen = Arc::new(Mutex::new(Seen::default()));
    let report = run(&f, &mut recovery, &seen);
    assert_eq!(
        ops(&seen),
        Vec::<String>::new(),
        "the quarantined set was read"
    );
    assert!(matches!(report.decode, PickDecode::Ready(_)));
    assert_eq!(recovery.quarantined().len(), 1, "the quarantine dropped");
}

/// The recovery state inside the selected set cannot expire its own
/// quarantine: a launch that crashes decoding `clear.wav` leaves a marker,
/// and the marker and quarantine commits write `sound-sets.toml` through a
/// temporary file in the data root, which changes that folder's times. The
/// identity follows a candidate's link only to a regular file of its
/// extension inside the set, so neither `fail.wav`, a junction to the set
/// folder itself, nor `scratch.wav`, a link to `sound-sets.toml`, nor
/// `fail.wav` as the folder holding the data root, carries those times.
// Covers: T18
#[test]
fn the_recovery_state_in_a_set_cannot_expire_its_quarantine() {
    let base = tmp();
    let set = base.path().join("set");
    let in_set = base.path().join("in-set");
    for (layout, set, data) in [
        (
            "the data root is the set",
            set.clone(),
            DataRoot::new(set.clone()),
        ),
        (
            "the data root is the set's fail.wav folder",
            in_set.clone(),
            DataRoot::new(in_set.join("fail.wav")),
        ),
    ] {
        put_bytes(&set, "clear.wav", &wav(480, 200));
        // The lock and the state file exist before any identity is read.
        drop(Recovery::open(&data, Box::new(DiskFs::default())).unwrap());
        if layout == "the data root is the set" {
            link_dir(&set.join("fail.wav"), &set);
            link_file(&set.join("scratch.wav"), &data.sound_sets());
        }
        let sets = scan(&set, SetRole::Sound, &ScanLimits::default()).sets;
        assert_eq!(sets, [canon(&set)], "{layout}");
        let the_pick = pick(Some(&set), None, &data.default_sounds());

        let crash = CrashAt {
            at: "clear.wav",
            state: data.sound_sets(),
            left: Mutex::new(None),
        };
        {
            let mut recovery = Recovery::open(&data, Box::new(DiskFs::default()))
                .unwrap()
                .recovery;
            let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
            decode_pick(
                &the_pick,
                &KeptSounds::default(),
                &budget,
                &mut recovery,
                &|| false,
                &crash,
            );
        }
        let left = crash.left.lock().unwrap().take();
        fs::write(data.sound_sets(), left.expect("clear.wav decoded")).unwrap();

        for launch in 2..=3 {
            let opened = Recovery::open(&data, Box::new(DiskFs::default())).unwrap();
            if launch == 2 {
                assert_eq!(
                    opened.quarantined.as_ref().map(|q| &q.path),
                    Some(&canon(&set)),
                    "{layout}"
                );
            }
            let mut recovery = opened.recovery;
            let chosen = choose(&sets, &mut recovery, &[], &mut |_| 0);
            assert_eq!(chosen, None, "{layout}, launch {launch}: the set chosen");
            let seen = Arc::new(Mutex::new(Seen::default()));
            let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
            decode_pick(
                &the_pick,
                &KeptSounds::default(),
                &budget,
                &mut recovery,
                &|| false,
                &LoggedDecode(Arc::clone(&seen)),
            );
            assert_eq!(
                ops(&seen),
                Vec::<String>::new(),
                "{layout}, launch {launch}: the quarantined set was read"
            );
            assert_eq!(
                recovery.quarantined().len(),
                1,
                "{layout}, launch {launch}: the quarantine dropped"
            );
        }
    }
}

/// A candidate that shares its file with another name, as a hard link
/// does, counts by its name alone: a set whose `scratch.wav` is a hard
/// link to a file rewritten in place at each launch (as the log is when
/// its rotation is blocked) stays quarantined across those launches. A
/// candidate with one name still expires it when edited (`recovery_cases`,
/// Q8).
// Covers: T18
#[test]
fn a_hard_link_to_a_written_file_cannot_expire_its_quarantine() {
    let f = fixture();
    let outside = tmp();
    let log = put_bytes(outside.path(), "tapestry.log", b"run 0\n");
    hard_link(&log, &f.set.join("scratch.wav"));

    let identity = identity_of(&f);
    {
        let mut r = Recovery::open(&f.data, Box::new(DiskFs::default()))
            .unwrap()
            .recovery;
        r.mark(&identity).unwrap();
    }
    for launch in 2..=4 {
        // Truncated and rewritten in place: longer, and later.
        let mut file = fs::File::create(&log).unwrap();
        io::Write::write_all(&mut file, "run\n".repeat(launch).as_bytes()).unwrap();
        let then = file.metadata().unwrap().modified().unwrap();
        let later = std::time::Duration::from_secs(61 * u64::try_from(launch).unwrap());
        file.set_modified(then + later).unwrap();
        drop(file);

        let opened = Recovery::open(&f.data, Box::new(DiskFs::default())).unwrap();
        if launch == 2 {
            assert_eq!(opened.quarantined.as_ref(), Some(&identity));
        }
        let mut recovery = opened.recovery;
        let seen = Arc::new(Mutex::new(Seen::default()));
        let report = run(&f, &mut recovery, &seen);
        assert_eq!(
            ops(&seen),
            Vec::<String>::new(),
            "launch {launch}: the quarantined set was read"
        );
        assert!(matches!(report.decode, PickDecode::Ready(_)));
        assert_eq!(
            recovery.quarantined(),
            std::slice::from_ref(&identity),
            "launch {launch}: the quarantine dropped"
        );
    }
}

/// Ending a hard link (a rename over its other name, as log rotation does)
/// changes the set's identity once, from the candidate's name alone to its
/// file's stamp. The name renamed over is then another file, so later
/// writes to it change nothing: a link made by hand costs at most one more
/// launch.
// Covers: T18
#[test]
fn ending_a_hard_link_changes_the_identity_once() {
    let f = fixture();
    let outside = tmp();
    let other = put_bytes(outside.path(), "tapestry.2.log", b"run 0\n");
    hard_link(&other, &f.set.join("scratch.wav"));
    let linked = identity_of(&f);

    let rotated = put_bytes(outside.path(), "tapestry.1.log", b"run 1\n");
    rename_over(&rotated, &other);
    let ended = identity_of(&f);
    assert_ne!(ended, linked, "ending the link left the identity");
    for i in 2..=4 {
        let rotated = put_bytes(
            outside.path(),
            "tapestry.1.log",
            "run\n".repeat(i).as_bytes(),
        );
        rename_over(&rotated, &other);
        put_bytes(
            outside.path(),
            "tapestry.2.log",
            "more\n".repeat(i).as_bytes(),
        );
        assert_eq!(identity_of(&f), ended, "rotation {i} changed the identity");
    }
}

/// A candidate whose count of names cannot be read counts by its name
/// alone, as with the disk's default probe: a size change leaves the
/// identity, and only a change of names makes another.
// Covers: T18
#[test]
fn a_candidate_with_no_count_counts_by_its_name() {
    let f = fixture();
    let unknown = game::sound_sets::DiskFs::default().links();
    let p = set_pick(&f);
    let of = |links: LinkCount| resolve(&p, links, &mut |_| false).sources[0].clone();
    let (before, counted) = (of(unknown), of(LINKS));
    put_bytes(&f.set, "fail.wav", &wav(250, -200));
    assert_eq!(of(unknown), before, "a size change read without a count");
    assert_ne!(of(LINKS), counted);
    put(&f.set, "scratch.wav");
    assert_ne!(of(unknown), before, "a new candidate left the identity");
}

fn identity_of_set(set: &Path) -> game::sound_sets::SourceIdentity {
    resolve(
        &pick(Some(set), None, Path::new("absent")),
        LINKS,
        &mut |_| false,
    )
    .sources[0]
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
    put_bytes(&b_set, "clear.wav", &wav(96, 50));
    // A byte copy of A's `scratch.wav`.
    put(&b_set, "o-close.wav");
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
    assert!(
        b_gen
            .sound(SystemKind::OptionClose)
            .is_some_and(|s| Arc::ptr_eq(s, kept)),
        "a copy of a kept sound's bytes does not play the kept sound"
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

/// A kept sound of 80 MiB and a file the new pick finds with the same bytes
/// fit a 128 MiB budget: the content is charged once, and the file shares
/// the kept sound.
#[test]
fn a_copy_of_a_kept_sound_is_charged_once() {
    const MIB: usize = 1024 * 1024;
    let f = fixture();
    let big = wav(40 * MIB, 7);
    put_bytes(&f.set, "scratch.wav", &big);
    let mut a_pick = set_pick(&f);
    a_pick.limits.max_sound_seconds = 1_000;
    let mut recovery = Recovery::open(&f.data, Box::new(DiskFs::default()))
        .unwrap()
        .recovery;
    let first = PickBudget::new(game::sound_sets::PICK_BYTES);
    let a = decode_pick(
        &a_pick,
        &KeptSounds::default(),
        &first,
        &mut recovery,
        &|| false,
        &OneFile,
    );
    let kept = Arc::clone(&a.kept.get(SystemKind::Scratch).expect("scratch").sound);
    assert_eq!(kept.bytes(), 80 * 1024 * 1024);

    // B has no `scratch` and a byte copy of it as `fail.wav`.
    let b_set = f.data.sound().join("b");
    put_bytes(&b_set, "clear.wav", &wav(96, 50));
    put_bytes(&b_set, "fail.wav", &big);
    drop(big);
    let mut b_pick = pick(Some(&b_set), None, &f.data.default_sounds());
    b_pick.limits.max_sound_seconds = 1_000;
    let budget = PickBudget::new(game::sound_sets::PICK_BYTES);
    let b = decode_pick(
        &b_pick,
        &a.kept,
        &budget,
        &mut recovery,
        &|| false,
        &OneFile,
    );
    assert_eq!(b.refused, [], "the copy was charged again");
    let PickDecode::Ready(b_gen) = b.decode else {
        panic!("{:?}", b.decode)
    };
    for kind in [SystemKind::Scratch, SystemKind::Fail] {
        assert!(
            b_gen.sound(kind).is_some_and(|s| Arc::ptr_eq(s, &kept)),
            "{kind:?} does not play the kept sound"
        );
    }
    let clear = b_gen.sound(SystemKind::Clear).unwrap().bytes();
    assert_eq!(budget.used(), kept.bytes() + clear);
    assert_eq!(b_gen.bytes(), kept.bytes() + clear);
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

/// Whether `path` is a symbolic link or junction itself.
fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// A file each run writes afresh is a new file with no other name: a link
/// to a set's file, a junction to a set's folder, a dangling link and
/// another file's second name are each removed, never written through, so
/// the set's files keep their bytes and times (design.md T18). A folder
/// at the name is refused. The file link cases need the right to make
/// one; without it they are skipped and the rest still run.
// Covers: T18
#[test]
fn a_fresh_file_never_writes_through_a_link() {
    let dir = tmp();
    let set = dir.path().join("set");
    let logs = dir.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    let scratch = put(&set, "scratch.wav");
    let stamp = |p: &Path| {
        let m = fs::metadata(p).unwrap();
        (fs::read(p).unwrap(), m.modified().unwrap())
    };
    let kept = stamp(&scratch);

    let written = |name: &str| {
        let path = logs.join(name);
        let mut file = game::create_fresh(&path).unwrap();
        io::Write::write_all(&mut file, b"run 1\n").unwrap();
        drop(file);
        assert!(!is_link(&path), "{name} is still a link");
        assert_eq!(fs::read(&path).unwrap(), b"run 1\n", "{name}");
    };

    // A junction to the set's folder.
    link_dir(&logs.join("junction.log"), &set);
    written("junction.log");
    assert_eq!(stamp(&scratch), kept, "the set's file through the junction");
    assert!(set.is_dir());

    // A second name of the set's file.
    fs::hard_link(&scratch, logs.join("hard.log")).unwrap();
    written("hard.log");
    assert_eq!(stamp(&scratch), kept, "the set's file through a hard link");

    // An old file of its own.
    put_bytes(&logs, "old.log", b"run 0, a longer line\n");
    written("old.log");

    // A folder.
    fs::create_dir(logs.join("folder.log")).unwrap();
    assert!(game::create_fresh(&logs.join("folder.log")).is_err());
    assert!(logs.join("folder.log").is_dir());

    // A link to the set's file, and a dangling one into the set.
    if link_file(&logs.join("linked.log"), &scratch).is_none() {
        println!("skipped the file links: no right to make one here");
        return;
    }
    written("linked.log");
    assert_eq!(stamp(&scratch), kept, "the set's file through a link");
    let missing = set.join("fail.wav");
    link_file(&logs.join("dangling.log"), &missing).unwrap();
    written("dangling.log");
    assert!(!missing.exists(), "the link's target was created");
}

/// A lock file is opened only once a link at its name is removed: a link
/// to a set's file, a dangling link into the set, a junction and a second
/// name of a set's file (counted by the probe). The set's files keep their
/// bytes, none is created, and none is held by the lock (design.md T18).
/// A lock file with one name keeps its bytes, so a lock another process
/// holds is never removed. The file link cases need the right to make
/// one; without it they are skipped and the rest still run.
// Covers: T18
#[test]
fn a_lock_never_lands_on_another_file() {
    let dir = tmp();
    let set = dir.path().join("set");
    let locks = dir.path().join("locks");
    fs::create_dir_all(&locks).unwrap();
    let scratch = put(&set, "scratch.wav");
    let kept = fs::read(&scratch).unwrap();
    let locked = |name: &str| {
        let path = locks.join(name);
        let file = game::open_lock(&path, LINKS).unwrap();
        file.try_lock().unwrap();
        assert!(!is_link(&path), "{name} is still a link");
        file
    };

    put_bytes(&locks, "own.lock", b"held");
    let own = locked("own.lock");
    let len = own.metadata().unwrap().len();
    assert_eq!(len, 4, "a lock file truncated or replaced");

    link_dir(&locks.join("junction.lock"), &set);
    let _junction = locked("junction.lock");
    assert!(set.is_dir());

    let other = put(&set, "clear.wav");
    hard_link(&other, &locks.join("hard.lock"));
    let _hard = locked("hard.lock");
    assert_eq!(
        fs::read(&other).ok().as_ref(),
        Some(&kept),
        "the set's file is held"
    );

    // Without a count, a second name is left as it is.
    put_bytes(&locks, "unknown.lock", b"held");
    let unknown = game::sound_sets::DiskFs::default().links();
    game::clear_link(&locks.join("unknown.lock"), unknown).unwrap();
    assert_eq!(fs::read(locks.join("unknown.lock")).unwrap(), b"held");

    if link_file(&locks.join("linked.lock"), &scratch).is_none() {
        println!("skipped the file links: no right to make one here");
        return;
    }
    let _linked = locked("linked.lock");
    assert_eq!(
        fs::read(&scratch).ok().as_ref(),
        Some(&kept),
        "the set's file is held"
    );
    let missing = set.join("fail.wav");
    link_file(&locks.join("dangling.lock"), &missing).unwrap();
    let _dangling = locked("dangling.lock");
    assert!(!missing.exists(), "the link's target was created");
}

/// With the data root in a set, `sound-sets.lock` as a dangling link into
/// the set does not add a candidate: the recovery state removes the link
/// and locks its own file, so the set's identity is unchanged (design.md
/// T18). Making the link needs the right to; without it the test is
/// skipped.
// Covers: T18
#[test]
fn the_recovery_lock_adds_no_candidate() {
    let base = tmp();
    let set = base.path().join("set");
    put_bytes(&set, "clear.wav", &wav(480, 200));
    let data = DataRoot::new(set.clone());
    let missing = set.join("scratch.wav");
    if link_file(&data.sound_sets_lock(), &missing).is_none() {
        println!("skipped: no right to make a file link here");
        return;
    }
    let before = identity_of_set(&set);
    let opened = Recovery::open(&data, Box::new(DiskFs::default())).unwrap();
    assert!(!missing.exists(), "the lock created scratch.wav in the set");
    assert!(!is_link(&data.sound_sets_lock()));
    assert_eq!(identity_of_set(&set), before);
    drop(opened);
}
