//! A folder of fixed files that every test process of a target shares.
//!
//! Under cargo-nextest each test is a process of its own, and many run at
//! once. A file one of them replaces is, on Windows, briefly absent to
//! another that opens it then, or refuses to open, so no file here is ever
//! replaced: the folder is named by its files' contents, and each file is
//! created whole (written aside, then moved to its name only if that name
//! is free). A process that finds a file already there, before or while
//! creating it, only reads it and checks it holds the bytes it would have
//! written.

#![allow(dead_code)]

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Cursor, ErrorKind, Write};
use std::path::{Path, PathBuf};

use image::{ImageFormat, RgbaImage};

/// `image` encoded as a PNG file.
pub fn png(image: &RgbaImage) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png).unwrap();
    bytes.into_inner()
}

/// The folder holding `files` (name, contents) under Cargo's temporary
/// folder for this target.
pub fn folder(prefix: &str, files: &[(String, Vec<u8>)]) -> PathBuf {
    folder_in(Path::new(env!("CARGO_TARGET_TMPDIR")), prefix, files)
}

/// The folder holding `files` under `base`: `<prefix>-<hash of files>`,
/// with each file created unless it is there already.
pub fn folder_in(base: &Path, prefix: &str, files: &[(String, Vec<u8>)]) -> PathBuf {
    folder_in_marked(base, prefix, files, &mut |_| {})
}

/// Where [`folder_in_marked`] is when it calls its mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Its first step, creating the folder, is done.
    AfterFirst,
    /// Its last step, placing the last file, comes next.
    BeforeLast,
}

/// [`folder_in`], calling `mark` between its steps: a time taken at
/// [`Mark::AfterFirst`] and one taken at [`Mark::BeforeLast`] lie strictly
/// between the placement's first file system step and its last.
pub fn folder_in_marked(
    base: &Path,
    prefix: &str,
    files: &[(String, Vec<u8>)],
    mark: &mut dyn FnMut(Mark),
) -> PathBuf {
    // `DefaultHasher::new` has fixed keys: every process of a build names
    // the same contents alike.
    let mut hasher = DefaultHasher::new();
    files.hash(&mut hasher);
    let dir = base.join(format!("{prefix}-{:016x}", hasher.finish()));
    std::fs::create_dir_all(&dir).unwrap();
    mark(Mark::AfterFirst);
    for (i, (name, bytes)) in files.iter().enumerate() {
        if i + 1 == files.len() {
            mark(Mark::BeforeLast);
        }
        place(&dir, name, bytes);
    }
    dir
}

/// Checks the file `dir/name` holds `bytes`, creating it when it is
/// absent. A folder already placed is only read: no process writes into
/// it again.
fn place(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    match std::fs::read(&path) {
        Ok(found) => check(&path, &found, bytes),
        Err(e) if e.kind() == ErrorKind::NotFound => create(dir, name, bytes),
        Err(e) => panic!("{path:?}: {e}"),
    }
}

/// Creates `dir/name` holding `bytes`: written aside, then moved to the
/// name only if it is free. When another process created it first, checks
/// it holds `bytes`. Never replaces a file.
pub fn create(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    let mut tmp = tempfile::NamedTempFile::new_in(dir).unwrap_or_else(|e| panic!("{dir:?}: {e}"));
    tmp.write_all(bytes).unwrap();
    match tmp.persist_noclobber(&path) {
        Ok(_) => {}
        Err(e) if e.error.kind() == ErrorKind::AlreadyExists => {
            let found = std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            check(&path, &found, bytes);
        }
        Err(e) => panic!("{path:?}: {}", e.error),
    }
}

fn check(path: &Path, found: &[u8], bytes: &[u8]) {
    assert!(
        found == bytes,
        "{path:?} holds other bytes than this build writes there"
    );
}
