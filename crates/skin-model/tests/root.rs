//! A [`Root`] reads and lists only inside the skin folder: names are
//! checked before they are normalised, `..` may not step above the folder,
//! links are followed and must stay inside, reads and listings are bounded
//! before any content is read or any entry examined.

use std::fs::{self, File};
use std::path::Path;

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

/// A directory junction on Windows, a symbolic link elsewhere.
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
        if !name.is_empty() {
            assert!(
                matches!(root.list(name, 16), Err(RootError::Rejected)),
                "list {name}"
            );
        }
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
    link_dir(&skin.join("bg/out"), &away);
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
    File::create(d.path().join("huge.json"))
        .unwrap()
        .set_len(1 << 40)
        .unwrap();
    let root = Root::new(d.path()).unwrap();
    assert_eq!(root.read("cap.json", 64).unwrap().0.len(), 64);
    assert!(matches!(
        root.read("over.json", 64),
        Err(RootError::TooLarge { len: 65, max: 64 })
    ));
    assert!(matches!(
        root.read("huge.json", 64),
        Err(RootError::TooLarge { len, max: 64 }) if len == 1 << 40
    ));
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
