//! One run owner (docs/test-device.md "Run Guard"): the evidence folder is
//! reserved before anything starts (`create_dir`: its parent must exist,
//! and an existing folder is refused), every evidence file in it is made
//! new (`create_new`), each stream is written one flushed line at a time
//! from any of the runner's threads, and a file is sealed (its sha256 in
//! the run's stream) when it is final.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use test_device::contract::hex;

/// Reserves the run's folder: it must not exist, its parent must.
///
/// # Errors
///
/// The folder exists, or it cannot be made.
pub fn reserve(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir(dir).with_context(|| {
        format!(
            "cannot reserve the run's folder {} (it must be new, its parent must exist)",
            dir.display()
        )
    })
}

/// A new evidence file.
///
/// # Errors
///
/// It exists, or it cannot be made.
pub fn create(path: &Path) -> anyhow::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("cannot create the new evidence file {}", path.display()))
}

/// A record stream shared by the runner's threads: each record a line of
/// its own, flushed as it is written. A failed write is kept and fails the
/// run at its end.
#[derive(Clone)]
pub struct Stream {
    inner: Arc<Mutex<Inner>>,
    pub path: PathBuf,
}

struct Inner {
    out: Option<BufWriter<File>>,
    failed: Option<String>,
}

impl Stream {
    /// A new stream at `path`.
    ///
    /// # Errors
    ///
    /// As [`create`].
    pub fn create(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                out: Some(BufWriter::new(create(path)?)),
                failed: None,
            })),
            path: path.to_owned(),
        })
    }

    /// Writes one record.
    pub fn write(&self, v: &Value) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(out) = g.out.as_mut() else {
            g.failed.get_or_insert_with(|| {
                format!("{}: a record after its end: {v}", self.path.display())
            });
            return;
        };
        let r = writeln!(out, "{v}").and_then(|()| out.flush());
        if let Err(e) = r {
            g.failed
                .get_or_insert_with(|| format!("writing {}: {e}", self.path.display()));
        }
    }

    /// Closes the stream: nothing more is written. Its first write error,
    /// if any.
    pub fn close(&self) -> Option<String> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mut out) = g.out.take()
            && let Err(e) = out.flush()
        {
            g.failed
                .get_or_insert_with(|| format!("writing {}: {e}", self.path.display()));
        }
        g.failed.clone()
    }

    /// The first write error, if any.
    pub fn failed(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .failed
            .clone()
    }
}

/// The sha256 of a file's bytes as they are now.
///
/// # Errors
///
/// It cannot be read.
pub fn digest(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    Ok(hex(&Sha256::digest(bytes)))
}

/// The seal of `path` (inside the run's folder `dir`) for run `run`'s role
/// `role`, as a `seal` record.
///
/// # Errors
///
/// The file cannot be read, or it lies outside `dir`.
pub fn seal(dir: &Path, path: &Path, run: &str, role: &str) -> anyhow::Result<Value> {
    let rel = path
        .strip_prefix(dir)
        .with_context(|| format!("{} is not in {}", path.display(), dir.display()))?;
    let rel: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Ok(json!({
        "t": "seal", "run": run, "role": role, "path": rel.join("/"), "sha256": digest(path)?,
    }))
}

/// `n` random bytes from the system's source, as lowercase hex.
///
/// # Errors
///
/// The source failed.
pub fn random_hex(n: usize) -> anyhow::Result<String> {
    use windows::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut buf = vec![0_u8; n];
    // SAFETY: the buffer is `n` bytes; the system's preferred generator.
    unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .context("BCryptGenRandom")?;
    Ok(hex(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_run_on_one_folder_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run");
        reserve(&dir).unwrap();
        let e = reserve(&dir).unwrap_err();
        assert!(format!("{e:#}").contains("must be new"), "{e:#}");
        // Its parent must exist.
        assert!(reserve(&root.path().join("no/run")).is_err());
    }

    #[test]
    fn an_evidence_file_that_exists_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run.jsonl");
        std::fs::write(&path, b"kept\n").unwrap();
        assert!(Stream::create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"kept\n");
    }

    #[test]
    fn a_stream_writes_flushed_lines_and_keeps_its_first_error() {
        let root = tempfile::tempdir().unwrap();
        let s = Stream::create(&root.path().join("run.jsonl")).unwrap();
        s.write(&json!({"t": "a"}));
        // Flushed: another reader sees it before the end.
        assert_eq!(std::fs::read_to_string(&s.path).unwrap(), "{\"t\":\"a\"}\n");
        assert_eq!(s.close(), None);
        s.write(&json!({"t": "b"}));
        assert!(s.failed().unwrap().contains("after its end"));
        let seal = seal(root.path(), &s.path, "r", "run").unwrap();
        assert_eq!(seal["path"], "run.jsonl");
        assert_eq!(random_hex(16).unwrap().len(), 32);
    }
}
