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
use test_device::evidence::{Process, RecorderReady, Run};

use super::link::{Connection, Endpoints};

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

/// The run an outer run in `set` reserved for `role` running `row`: the
/// outer's run id (the inner's set), the inner's run id and its folder,
/// which is made here (`create_dir`, so a role runs once). The reservation
/// is the outer's `start` record, the first line of its `run.jsonl`.
///
/// # Errors
///
/// No such reservation, or the folder exists.
pub fn reserved(set: &Path, role: &str, row: &str) -> anyhow::Result<(String, String, PathBuf)> {
    let text = std::fs::read_to_string(set.join(test_device::contract::RUN))
        .with_context(|| format!("the reserving run in {}", set.display()))?;
    let first = text.lines().next().unwrap_or_default();
    let Ok(Run::Start(start)) = serde_json::from_str::<Run>(first) else {
        anyhow::bail!("{}'s first record is not a `start`", set.display());
    };
    let outer = start.run;
    let Some(inner) = start.inner.filter(|r| r.role == role) else {
        anyhow::bail!("the run in {} reserved no role `{role}`", set.display());
    };
    if inner.row != row {
        anyhow::bail!(
            "role `{role}` is reserved for row `{}`, not `{row}`",
            inner.row
        );
    }
    let dir = set.join(&inner.dir);
    reserve(&dir)?;
    Ok((outer, inner.run, dir))
}

/// Checks the recorder's `ready` against the child the runner spawned: the
/// run the runner gave it, its process, and its window, which must be that
/// process's.
///
/// # Errors
///
/// Another process's record or window.
pub fn ready_is_the_child_s(
    ready: &RecorderReady,
    run: &str,
    spawned: Process,
    window_pid: u32,
) -> Result<(), String> {
    if ready.run != run {
        return Err(format!(
            "the recorder's `ready` names run `{}`, not this run, `{run}`",
            ready.run
        ));
    }
    if ready.process != spawned {
        return Err(format!(
            "the recorder's `ready` names process {:?}, not the child spawned, {spawned:?}",
            ready.process
        ));
    }
    if window_pid != spawned.pid {
        return Err(format!(
            "the recorder's window {} is process {window_pid}'s, not the child's ({})",
            ready.window, spawned.pid
        ));
    }
    Ok(())
}

/// A new connection to the device at the bound location, its `hello`
/// checked against `build`: after a reboot or a reconnect, the device must
/// come back at the same location.
///
/// # Errors
///
/// The device is absent or elsewhere, or its `hello` is refused.
pub fn rediscover<E: Endpoints + ?Sized>(
    ep: &E,
    bound: &str,
    build: &str,
) -> anyhow::Result<Connection> {
    let mut c = Connection::bind(ep, Some(bound))?;
    c.check_hello(Some(build))?;
    Ok(c)
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

    fn ready(pid: u32, window: u64) -> RecorderReady {
        RecorderReady {
            q: 1,
            run: "r".into(),
            process: Process {
                pid,
                created: u64::from(pid) * 10,
            },
            window,
            location: "L".into(),
            keyboard: None,
            pad: None,
        }
    }

    #[test]
    fn a_ready_window_of_another_process_is_refused() {
        let child = Process {
            pid: 40,
            created: 400,
        };
        assert_eq!(ready_is_the_child_s(&ready(40, 9), "r", child, 40), Ok(()));
        // Another run's recorder.
        assert!(ready_is_the_child_s(&ready(40, 9), "s", child, 40).is_err());
        // Its window is another process's.
        assert!(ready_is_the_child_s(&ready(40, 9), "r", child, 41).is_err());
        // Another process's record (a reused id: another creation time).
        assert!(ready_is_the_child_s(&ready(41, 9), "r", child, 41).is_err());
        let reused = Process {
            pid: 40,
            created: 401,
        };
        assert!(ready_is_the_child_s(&ready(40, 9), "r", reused, 40).is_err());
    }

    #[test]
    fn a_device_back_on_another_port_is_refused() {
        use super::super::link::fake::{Dev, Host, hello};
        const HERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)";
        const ELSEWHERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(4)";
        let at = |loc: &str| Host {
            devs: vec![Dev::new(loc, "COM5", hello("abc", false))],
            ..Host::default()
        };
        assert!(rediscover(&at(HERE), HERE, "abc").is_ok());
        let e = rediscover(&at(ELSEWHERE), HERE, "abc").err().unwrap();
        assert!(format!("{e:#}").contains(HERE), "{e:#}");
        // Back at the location, but another build.
        assert!(rediscover(&at(HERE), HERE, "abd").is_err());
    }

    fn outer_start(dir: &Path, inner: &serde_json::Value) {
        let start = json!({
            "t": "start", "run": "a".repeat(32), "row": "sink-kill", "role": "run",
            "build": "0123456789ab", "process": {"pid": 1, "created": 2},
            "commit": "0123456789ab", "tag": 7, "cut": 1_073_741_825, "args": [], "set": null, "inner": inner,
        });
        std::fs::write(dir.join("run.jsonl"), format!("{start}\n")).unwrap();
    }

    #[test]
    fn an_inner_runs_under_the_outer_s_reservation() {
        let root = tempfile::tempdir().unwrap();
        let set = root.path();
        let id = "b".repeat(32);
        outer_start(
            set,
            &json!({"role": "inner", "row": "sink", "run": id, "dir": "inner"}),
        );
        let (outer, run, dir) = reserved(set, "inner", "sink").unwrap();
        assert_eq!(outer, "a".repeat(32));
        assert_eq!(run, id);
        assert_eq!(dir, set.join("inner"));
        assert!(dir.is_dir());
        // A role runs once.
        assert!(reserved(set, "inner", "sink").is_err());
        // Another role or row is not reserved.
        trash_dir(&dir);
        assert!(reserved(set, "other", "sink").is_err());
        assert!(reserved(set, "inner", "p1").is_err());
        // No reservation at all.
        outer_start(set, &serde_json::Value::Null);
        assert!(reserved(set, "inner", "sink").is_err());
    }

    fn trash_dir(dir: &Path) {
        std::fs::remove_dir(dir).unwrap();
    }
}
