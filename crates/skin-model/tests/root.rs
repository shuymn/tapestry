//! A [`Root`] reads and lists only inside the skin folder: names are
//! checked before they are normalised, `..` may not step above the folder,
//! links are followed and must stay inside, reads and listings are bounded
//! before any content is read or any entry examined.

mod image_support;

use std::fs::{self, File};
use std::path::Path;

use image_support::link_dir;
use skin_model::{Entry, MAX_PATH_BYTES, Root, RootError};
use tempfile::TempDir;

fn dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn write(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn names_resolve_inside_the_folder_after_normalising() {
    let d = dir();
    write(d.path(), "_font/x.ttf", b"font");
    write(d.path(), "play/s.json", b"{}");
    let root = Root::new(d.path()).unwrap();
    let (bytes, id) = root.read("play/../_font/x.ttf", 16).unwrap();
    assert_eq!(bytes, b"font");
    assert_eq!(id.name(), "_font/x.ttf");
    let (_, other) = root.read("_font\\.\\x.ttf", 16).unwrap();
    assert_eq!(id, other);
    assert_eq!(
        root.read("play/../../x", 16).unwrap_err().to_string(),
        RootError::Underflow.to_string()
    );
    assert!(matches!(
        root.read("..", 16).unwrap_err(),
        RootError::Underflow
    ));
    // A name that normalises to the folder itself is no file.
    assert!(matches!(
        root.read("play/..", 16).unwrap_err(),
        RootError::NotAFile
    ));
}

#[test]
fn rooted_drive_unc_and_verbatim_names_are_refused_before_normalising() {
    let d = dir();
    write(d.path(), "x", b"x");
    let root = Root::new(d.path()).unwrap();
    let long = "a".repeat(MAX_PATH_BYTES + 1);
    let absolute = d.path().join("x");
    for name in [
        absolute.to_str().unwrap(),
        "/x",
        "\\x",
        "\\\\server\\s\\x",
        "//server/s/x",
        "\\\\?\\C:\\x",
        "//?/C:/x",
        "C:\\x",
        "c:x",
        "x:stream",
        "",
        long.as_str(),
    ] {
        assert!(
            matches!(root.read(name, 16), Err(RootError::Rejected)),
            "read {name}"
        );
        assert!(
            matches!(Root::check_name(name), Err(RootError::Rejected)),
            "check {name}"
        );
        if !name.is_empty() {
            assert!(
                matches!(root.list(name, 16), Err(RootError::Rejected)),
                "list {name}"
            );
        }
    }
    let longest = "a".repeat(MAX_PATH_BYTES);
    for name in ["x", "../x", "a/b*.png", longest.as_str()] {
        assert!(Root::check_name(name).is_ok(), "check {name}");
    }
}

#[test]
fn links_are_followed_and_must_stay_inside() {
    let outer = dir();
    let skin = outer.path().join("skin");
    let away = outer.path().join("away");
    write(&skin, "real.json", b"in");
    write(&away, "x.json", b"out");
    link_dir(&skin.join("out"), &away);
    link_dir(&skin.join("in"), &skin);
    let root = Root::new(&skin).unwrap();
    assert!(matches!(
        root.read("out/x.json", 16),
        Err(RootError::Escapes)
    ));
    assert_eq!(root.read("in/real.json", 16).unwrap().0, b"in");
    // An outward folder is refused before it is enumerated: a budget of
    // zero entries would fail on its first entry.
    assert!(matches!(root.list("out", 0), Err(RootError::Escapes)));
    let inner = root.list("in", 16).unwrap();
    assert!(inner.entries.iter().any(|e| e.name == "real.json"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(away.join("x.json"), skin.join("f.json")).unwrap();
        assert!(matches!(root.read("f.json", 16), Err(RootError::Escapes)));
        std::os::unix::fs::symlink(skin.join("real.json"), skin.join("g.json")).unwrap();
        let (bytes, id) = root.read("g.json", 16).unwrap();
        assert_eq!(bytes, b"in");
        // The identity is the canonical file, not the name.
        assert_eq!(id, root.read("real.json", 16).unwrap().1);
        assert_eq!(id.name(), "g.json");
    }
}

#[test]
fn a_listing_skips_links_out_and_counts_them() {
    let outer = dir();
    let skin = outer.path().join("skin");
    let away = outer.path().join("away");
    write(&skin, "bg/b.png", b"");
    write(&skin, "bg/a.png", b"");
    fs::create_dir_all(skin.join("bg/sub")).unwrap();
    write(&away, "x.png", b"");
    link_dir(&skin.join("bg").join("out"), &away);
    let root = Root::new(&skin).unwrap();
    let listing = root.list("bg", 16).unwrap();
    assert_eq!(
        listing.entries,
        [
            Entry {
                name: "a.png".into(),
                folder: false
            },
            Entry {
                name: "b.png".into(),
                folder: false
            },
            Entry {
                name: "sub".into(),
                folder: true
            },
        ]
    );
    assert_eq!(listing.enumerated, 4);
    assert_eq!(root.list("", 16).unwrap().entries.len(), 1);
    assert!(matches!(
        root.list("bg/a.png", 16),
        Err(RootError::NotAFolder)
    ));
    assert!(matches!(root.list("none", 16), Err(RootError::Missing)));
}

#[test]
fn a_listing_counts_every_entry_before_examining_it() {
    let d = dir();
    for i in 0..8 {
        write(d.path(), &format!("g/{i}.txt"), b"");
    }
    let root = Root::new(d.path()).unwrap();
    assert_eq!(root.list("g", 8).unwrap().enumerated, 8);
    assert!(matches!(
        root.list("g", 7),
        Err(RootError::TooManyEntries { max: 7 })
    ));
}

#[test]
fn reads_are_bounded_before_reading() {
    let d = dir();
    write(d.path(), "cap.json", &[b' '; 64]);
    // Sparse files: their length, not their blocks. Zeros would read back
    // as content, and reading a terabyte would not finish.
    File::create(d.path().join("over.json"))
        .unwrap()
        .set_len(65)
        .unwrap();
    let root = Root::new(d.path()).unwrap();
    assert_eq!(root.read("cap.json", 64).unwrap().0.len(), 64);
    assert!(matches!(
        root.read("over.json", 64),
        Err(RootError::TooLarge { len: 65, max: 64 })
    ));
    // NTFS allocates a file extended by `set_len`, so only here.
    #[cfg(unix)]
    {
        File::create(d.path().join("huge.json"))
            .unwrap()
            .set_len(1 << 40)
            .unwrap();
        assert!(matches!(
            root.read("huge.json", 64),
            Err(RootError::TooLarge { len, max: 64 }) if len == 1 << 40
        ));
    }
}

#[test]
fn only_regular_files_are_read() {
    let d = dir();
    fs::create_dir(d.path().join("x.json")).unwrap();
    let root = Root::new(d.path()).unwrap();
    assert!(matches!(root.read("x.json", 16), Err(RootError::NotAFile)));
    assert!(matches!(
        root.read("none.json", 16),
        Err(RootError::Missing)
    ));
    assert!(matches!(
        Root::new(&d.path().join("none")),
        Err(RootError::Folder(_))
    ));
}

/// Opening a FIFO with no writer blocks, so the file type must be checked
/// before the file is opened. The read runs on a thread with a deadline.
#[cfg(unix)]
#[test]
fn a_fifo_is_refused_without_opening_it() {
    use std::sync::mpsc;
    use std::time::Duration;

    let d = dir();
    let status = std::process::Command::new("mkfifo")
        .arg(d.path().join("p.json"))
        .status()
        .expect("this test needs `mkfifo`");
    assert!(status.success());
    let root = Root::new(d.path()).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(root.read("p.json", 16).map(|_| ()));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => assert!(matches!(result, Err(RootError::NotAFile)), "{result:?}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("reading a FIFO blocked: it was opened before its type was checked");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the reading thread panicked"),
    }
}

/// What reading `name` gave: the bytes, or the refusal's variant.
fn outcome(root: &Root, name: &str) -> Result<Vec<u8>, String> {
    root.read(name, 64)
        .map(|(bytes, _)| bytes)
        .map_err(|e| format!("{e:?}"))
}

/// A name is walked from the folder one component at a time, each link
/// judged by its own target before anything below it is looked at
/// (design.md "Threat Model" T08). An outward link refuses its name
/// whatever lies beyond it: a way back inside, an absent entry, a
/// dangling child outside, or nothing, also below nested folders; an
/// inward link is followed; an unresolved one (missing target, a target
/// asking for a child under a file, a chain over the hop cap, a loop)
/// is `Dangling`; a name is `Missing` only when its own components run
/// into an absent entry or a file.
// Covers: T08
#[test]
fn a_name_is_walked_from_the_folder_and_stops_at_an_outward_link() {
    let outer = dir();
    let skin = outer.path().join("skin");
    let ext = outer.path().join("ext");
    write(&skin, "real/x.lua", b"inside");
    fs::create_dir_all(skin.join("a/b")).unwrap();
    write(&ext, "x.lua", b"outside");
    // O and N: outward links, at the top and below two real folders; the
    // outside holds a way back in and a dangling child (D).
    link_dir(&skin.join("out"), &ext);
    link_dir(&skin.join("a").join("b").join("out"), &ext);
    link_dir(&ext.join("back"), &skin.join("real"));
    link_dir(&ext.join("gone"), &ext.join("nothing"));
    // I, ID, R, F.
    link_dir(&skin.join("in"), &skin.join("real"));
    link_dir(&skin.join("gone"), &skin.join("nothing"));
    link_dir(&skin.join("return"), &skin.join("out").join("back"));
    link_dir(
        &skin.join("badtarget"),
        &skin.join("real").join("x.lua").join("child"),
    );
    link_dir(&skin.join("tofile"), &skin.join("real").join("x.lua"));
    // L: 32 links in a chain, 33, a self-loop and a two-link loop.
    for (prefix, n) in [("c", 32), ("d", 33)] {
        for i in 0..n {
            let target = if i + 1 == n {
                skin.join("real")
            } else {
                skin.join(format!("{prefix}{}", i + 1))
            };
            link_dir(&skin.join(format!("{prefix}{i}")), &target);
        }
    }
    link_dir(&skin.join("self"), &skin.join("self"));
    link_dir(&skin.join("p"), &skin.join("q"));
    link_dir(&skin.join("q"), &skin.join("p"));
    let root = Root::new(&skin).unwrap();
    let err = |e: &str| Err::<Vec<u8>, String>(e.to_owned());

    for top in ["", "a/b/"] {
        for rest in [
            "out/back/x.lua",
            "out/back/none.lua",
            "out/absent/none.lua",
            "out/gone/none.lua",
            "out/x.lua",
            "out/none.lua",
        ] {
            let name = format!("{top}{rest}");
            assert_eq!(outcome(&root, &name), err("Escapes"), "{name}");
        }
    }
    assert_eq!(outcome(&root, "return/x.lua"), err("Escapes"));
    assert_eq!(outcome(&root, "in/x.lua"), Ok(b"inside".to_vec()));
    assert_eq!(
        root.read("in/x.lua", 64).unwrap().1,
        root.read("real/x.lua", 64).unwrap().1
    );
    assert_eq!(outcome(&root, "in/none.lua"), err("Missing"));
    assert_eq!(outcome(&root, "gone/x.lua"), err("Dangling"));
    assert_eq!(outcome(&root, "badtarget/x.lua"), err("Dangling"));
    assert_eq!(outcome(&root, "tofile/child"), err("Missing"));
    assert_eq!(outcome(&root, "tofile"), Ok(b"inside".to_vec()));
    assert_eq!(outcome(&root, "real/x.lua/child"), err("Missing"));
    assert_eq!(outcome(&root, "a/none/x.lua"), err("Missing"));
    assert_eq!(outcome(&root, "c0/x.lua"), Ok(b"inside".to_vec()));
    assert_eq!(outcome(&root, "d0/x.lua"), err("Dangling"));
    assert_eq!(outcome(&root, "self/x.lua"), err("Dangling"));
    assert_eq!(outcome(&root, "p/x.lua"), err("Dangling"));
    #[cfg(windows)]
    assert_eq!(outcome(&root, "OUT/none.lua"), err("Escapes"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("../real", skin.join("a/rel")).unwrap();
        assert_eq!(outcome(&root, "a/rel/x.lua"), Ok(b"inside".to_vec()));
    }

    let names: Vec<String> = root
        .list("", 1_000)
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.name)
        .collect();
    let has = |n: &str| names.iter().any(|e| e == n);
    assert!(!has("return"), "{names:?}");
    for skipped in ["out", "gone", "badtarget", "d0", "self", "p", "q"] {
        assert!(!has(skipped), "{skipped}: {names:?}");
    }
    for kept in ["in", "c0", "real", "a"] {
        assert!(has(kept), "{kept}: {names:?}");
    }
    assert!(matches!(root.list("out/back", 16), Err(RootError::Escapes)));
    assert!(matches!(root.list("a/b/out", 16), Err(RootError::Escapes)));
}

/// A file link `link` to the target spelled `target` (`/` separators,
/// written with `\` on Windows), relative or absolute as spelled.
fn file_link(link: &Path, target: &str) {
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(target.replace('/', "\\"), link).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// A folder link `link` to the relative target `target`, as
/// [`file_link`] spells it.
fn folder_link(link: &Path, target: &str) {
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target.replace('/', "\\"), link).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// A skin folder `skin` beside `ext`, whose link targets put a `..` after
/// a component the walk must judge first:
/// - `probe` -> `out/../x.lua`, `out` an outward link to `ext/sub`: on
///   Unix the system reads `ext/x.lua` through it;
/// - `miss` -> `missing/../x.lua`, `missing` absent;
/// - `up` -> `../ext/x.lua`, above the folder;
/// - `deep` -> `sub/../../ext/x.lua`, `sub` an inward link to `a/b`:
///   the walk reaches the folder's own `ext/x.lua`, and so does Unix;
///   Windows resolves the spelling, which leads to the outside `ext/x.lua`;
/// - `a/rel` -> `../real`, an ordinary inward target.
///
/// `outside` says whether the outside `ext/x.lua` exists.
fn dot_dot_links(outside: bool) -> (TempDir, std::path::PathBuf) {
    let outer = dir();
    let skin = outer.path().join("skin");
    let ext = outer.path().join("ext");
    write(&skin, "x.lua", b"inside");
    write(&skin, "real/x.lua", b"real");
    write(&skin, "ext/x.lua", b"inside-ext");
    fs::create_dir_all(skin.join("a/b")).unwrap();
    fs::create_dir_all(ext.join("sub")).unwrap();
    if outside {
        write(&ext, "x.lua", b"outside");
    }
    link_dir(&skin.join("out"), &ext.join("sub"));
    link_dir(&skin.join("sub"), &skin.join("a").join("b"));
    file_link(&skin.join("probe"), "out/../x.lua");
    file_link(&skin.join("miss"), "missing/../x.lua");
    file_link(&skin.join("up"), "../ext/x.lua");
    file_link(&skin.join("deep"), "sub/../../ext/x.lua");
    folder_link(&skin.join("a").join("rel"), "../real");
    (outer, skin)
}

/// A link target's `..` steps up only once the components before it are
/// walked, so an outward link before it refuses the name whatever lies
/// outside, and a `..` above the folder leads out of it; an ordinary
/// inward `../real` and a `..` after an inward link stay admitted
/// (design.md "Threat Model" T08).
// Covers: T08
#[test]
fn a_target_steps_up_only_after_the_link_before_it_is_judged() {
    for outside in [true, false] {
        let (_outer, skin) = dot_dot_links(outside);
        let root = Root::new(&skin).unwrap();
        let err = |e: &str| Err::<Vec<u8>, String>(e.to_owned());
        assert_eq!(outcome(&root, "probe"), err("Escapes"), "outside {outside}");
        assert_eq!(outcome(&root, "up"), err("Escapes"), "outside {outside}");
        assert_eq!(
            outcome(&root, "deep"),
            Ok(b"inside-ext".to_vec()),
            "outside {outside}"
        );
        assert_eq!(outcome(&root, "a/rel/x.lua"), Ok(b"real".to_vec()));
        // What the system itself reads through `probe`: Unix follows `out`
        // first, out of the folder; Windows resolves the spelling.
        #[cfg(unix)]
        assert_eq!(
            fs::read(skin.join("probe")).ok(),
            outside.then(|| b"outside".to_vec())
        );
        #[cfg(windows)]
        assert_eq!(fs::read(skin.join("probe")).unwrap(), b"inside");
    }
}

/// An unresolved component before a target's `..` leaves the link
/// unresolved, though the spelling would cancel it (design.md "Threat
/// Model" T08).
// Covers: T08
#[test]
fn an_unresolved_component_before_a_dot_dot_is_dangling() {
    let (_outer, skin) = dot_dot_links(true);
    let root = Root::new(&skin).unwrap();
    assert_eq!(outcome(&root, "miss"), Err("Dangling".to_owned()));
}

/// A listing keeps a link by its walk and describes it by the path the
/// walk reached, so it is the same whether the outside file exists or
/// not: the outward and unresolved `..` links are left out, the inward
/// ones kept (design.md "Threat Model" T08).
// Covers: T08
#[test]
fn a_listing_of_links_does_not_depend_on_what_lies_outside() {
    let listings: Vec<(Vec<Entry>, Vec<Entry>)> = [true, false]
        .into_iter()
        .map(|outside| {
            let (_outer, skin) = dot_dot_links(outside);
            let root = Root::new(&skin).unwrap();
            (
                root.list("", 64).unwrap().entries,
                root.list("a", 64).unwrap().entries,
            )
        })
        .collect();
    for (top, a) in &listings {
        let names: Vec<&str> = top.iter().map(|e| e.name.as_str()).collect();
        for skipped in ["probe", "miss", "up", "out"] {
            assert!(!names.contains(&skipped), "{skipped}: {names:?}");
        }
        let entry = |name: &str, folder| Entry {
            name: name.to_owned(),
            folder,
        };
        assert!(top.contains(&entry("deep", false)), "{top:?}");
        assert!(top.contains(&entry("sub", true)), "{top:?}");
        assert!(a.contains(&entry("rel", true)), "{a:?}");
    }
    assert_eq!(listings.first(), listings.last());
}
