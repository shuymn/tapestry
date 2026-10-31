//! Skin folders written by the tests: self-made definitions only.

#![allow(dead_code)]

use std::fs;
use std::path::Path;

use skin_beatoraja::{Header, ImportError, InputLimit, read_header};
use tempfile::TempDir;

/// A skin folder holding `files`, each a name and its text.
pub fn skin(files: &[(&str, &str)]) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    for (name, text) in files {
        write(d.path(), name, text.as_bytes());
    }
    d
}

pub fn write(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// The header of `s.json` with no option enabled.
pub fn header(d: &TempDir) -> Result<Header, ImportError> {
    header_of(d.path(), "s.json")
}

pub fn header_of(root: &Path, file: &str) -> Result<Header, ImportError> {
    read_header(root, file).map(|(_, h)| h)
}

/// The option names of a header.
pub fn names(h: &Header) -> Vec<String> {
    h.options
        .iter()
        .map(|o| o.name.clone().unwrap_or_default())
        .collect()
}

/// The input bound an import stopped at.
pub fn limit(result: Result<Header, ImportError>) -> InputLimit {
    match result {
        Err(ImportError::Input { limit, .. }) => limit,
        other => panic!("{other:?}"),
    }
}

/// A play skin whose fields are `body`, a list of members.
pub fn play(body: &str) -> String {
    format!("{{\"type\": 0, {body}}}")
}

/// `text` padded with spaces to exactly `len` bytes.
pub fn padded(text: &str, len: usize) -> Vec<u8> {
    let mut b = text.as_bytes().to_vec();
    assert!(b.len() <= len);
    b.resize(len, b' ');
    b
}

/// A directory junction on Windows, a symbolic link elsewhere.
pub fn link_dir(link: &Path, target: &Path) {
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
