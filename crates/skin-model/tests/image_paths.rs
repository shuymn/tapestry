//! A skin's file names reach only regular files inside the skin folder:
//! names that leave it, links out of it, non-regular files and files over
//! the size cap are refused before their content is read.

mod image_support;

use std::fs::File;
use std::path::Path;

use image_support::{dir, file, load, load_files, save, solid, whole};
use skin_model::{FileAt, ImageError, MAX_FILE_BYTES, MAX_PATH_BYTES};

const AT0: FileAt = FileAt { file: 0, hi: false };

#[test]
fn names_that_leave_the_folder_are_rejected() {
    let outer = dir();
    let root = outer.path().join("skin");
    std::fs::create_dir_all(root.join("a")).unwrap();
    // Every target exists, so only the name check can refuse them.
    save(outer.path(), "x.png", &solid(1, 1, [255; 4]));
    save(&root, "x.png", &solid(1, 1, [255; 4]));
    let long = format!("{}.png", "a".repeat(MAX_PATH_BYTES + 1 - 4));
    assert_eq!(long.len(), MAX_PATH_BYTES + 1);
    let unc = format!("\\\\server\\s\\{}", "x.png");
    for name in [
        "../x.png",
        "a/../../x.png",
        "/x.png",
        "\\x.png",
        unc.as_str(),
        "C:\\x.png",
        "c:x.png",
        "x.png:s",
        "",
        long.as_str(),
    ] {
        let err = load_files(&root, &[name]).unwrap_err();
        assert!(
            matches!(err, ImageError::PathRejected { at: AT0 }),
            "{name}: {err:?}"
        );
    }
    // The @2x name is checked the same way.
    let mut def = file("x.png");
    def.hi = Some("../x.png".into());
    let err = load(&root, vec![def], vec![whole(0)]).unwrap_err();
    assert!(matches!(
        err,
        ImageError::PathRejected {
            at: FileAt { file: 0, hi: true }
        }
    ));
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
fn links_out_of_the_folder_escape_and_links_inside_load() {
    let outer = dir();
    let root = outer.path().join("skin");
    let away = outer.path().join("away");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&away).unwrap();
    save(&away, "x.png", &solid(2, 2, [255; 4]));
    save(&root, "real.png", &solid(2, 2, [255; 4]));
    link_dir(&root.join("link"), &away);
    let err = load_files(&root, &["link/x.png"]).unwrap_err();
    assert!(
        matches!(err, ImageError::PathEscapes { at: AT0 }),
        "{err:?}"
    );
    // A link to a folder inside loads.
    link_dir(&root.join("inner"), &root);
    load_files(&root, &["inner/real.png"]).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(away.join("x.png"), root.join("f.png")).unwrap();
        let err = load_files(&root, &["f.png"]).unwrap_err();
        assert!(
            matches!(err, ImageError::PathEscapes { at: AT0 }),
            "{err:?}"
        );
        std::os::unix::fs::symlink(root.join("real.png"), root.join("g.png")).unwrap();
        load_files(&root, &["g.png"]).unwrap();
        // A device outside the folder is refused as an escape too.
        std::os::unix::fs::symlink("/dev/zero", root.join("z.png")).unwrap();
        let err = load_files(&root, &["z.png"]).unwrap_err();
        assert!(
            matches!(err, ImageError::PathEscapes { at: AT0 }),
            "{err:?}"
        );
    }
}

#[test]
fn only_regular_files_are_read() {
    let d = dir();
    std::fs::create_dir(d.path().join("x.png")).unwrap();
    let err = load_files(d.path(), &["x.png"]).unwrap_err();
    assert!(matches!(err, ImageError::NotAFile { at: AT0 }), "{err:?}");
    #[cfg(unix)]
    {
        let _socket =
            std::os::unix::net::UnixListener::bind(short(d.path()).join("s.png")).unwrap();
        let err = load_files(d.path(), &["s.png"]).unwrap_err();
        assert!(matches!(err, ImageError::NotAFile { at: AT0 }), "{err:?}");
    }
}

/// Folder `path`, or the same folder relative to the working folder when
/// that is shorter: a socket's path has a small length limit.
#[cfg(unix)]
fn short(path: &Path) -> std::path::PathBuf {
    let path = path.canonicalize().unwrap();
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let common = path
        .components()
        .zip(cwd.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut rel = std::path::PathBuf::new();
    for _ in cwd.components().skip(common) {
        rel.push("..");
    }
    rel.extend(path.components().skip(common));
    if rel.as_os_str().len() < path.as_os_str().len() {
        rel
    } else {
        path
    }
}

/// Opening a FIFO with no writer blocks, so the file type must be checked
/// before the file is opened. The load runs on a thread with a deadline.
#[cfg(unix)]
#[test]
fn a_fifo_is_refused_without_opening_it() {
    use std::sync::mpsc;
    use std::time::Duration;

    let d = dir();
    let fifo = d.path().join("p.png");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("this test needs `mkfifo`");
    assert!(status.success());
    let root = d.path().to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(load_files(&root, &["p.png"]).map(|_| ()));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => {
            let err = result.unwrap_err();
            assert!(matches!(err, ImageError::NotAFile { at: AT0 }), "{err:?}");
        }
        Err(_) => {
            // Release the blocked open before failing.
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
            panic!("loading a FIFO blocked: it was opened before its type was checked");
        }
    }
}

#[test]
fn files_over_the_size_cap_are_refused_before_reading() {
    let d = dir();
    // Sparse files: their length, not their blocks.
    File::create(d.path().join("big.png"))
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    let err = load_files(d.path(), &["big.png"]).unwrap_err();
    assert!(
        matches!(err, ImageError::FileTooLarge { at: AT0 }),
        "{err:?}"
    );
    // At the cap the length passes; zeros are no image.
    File::create(d.path().join("cap.png"))
        .unwrap()
        .set_len(MAX_FILE_BYTES)
        .unwrap();
    let err = load_files(d.path(), &["cap.png"]).unwrap_err();
    assert!(
        matches!(err, ImageError::Unsupported { at: AT0 }),
        "{err:?}"
    );
}

#[test]
fn a_missing_file_is_reported() {
    let d = dir();
    let err = load_files(d.path(), &["none.png"]).unwrap_err();
    assert!(matches!(err, ImageError::Missing { at: AT0 }), "{err:?}");
}
