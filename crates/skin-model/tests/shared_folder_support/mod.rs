//! A folder of fixed files that every test process of a target shares.
//!
//! Under cargo-nextest each test is a process of its own, and many run at
//! once. A file one of them replaces is, on Windows, briefly absent to
//! another that opens it then, or refuses to open, so no file here is ever
//! replaced: the folder is named by its files' contents, and each file is
//! created whole (written aside, then moved to its name only if that name
//! is free). A process that finds a file already there reads it back and
//! checks it holds the bytes it would have written.

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
    // `DefaultHasher::new` has fixed keys: every process of a build names
    // the same contents alike.
    let mut hasher = DefaultHasher::new();
    files.hash(&mut hasher);
    let dir = base.join(format!("{prefix}-{:016x}", hasher.finish()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, bytes) in files {
        place(&dir, name, bytes);
    }
    dir
}

/// Creates `dir/name` holding `bytes`, or checks the file already there
/// holds them. Never replaces a file.
fn place(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    let mut tmp = tempfile::NamedTempFile::new_in(dir).unwrap();
    tmp.write_all(bytes).unwrap();
    match tmp.persist_noclobber(&path) {
        Ok(_) => return,
        Err(e) if e.error.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => panic!("{path:?}: {}", e.error),
    }
    let found = std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    assert!(
        found == bytes,
        "{path:?} holds other bytes than this build writes there"
    );
}
