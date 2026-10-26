//! Reading paths and contents from the Git index.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, anyhow, bail};

/// One index entry (`git ls-files -s`).
#[derive(Debug)]
pub struct Entry {
    pub mode: String,
    pub oid: String,
    pub stage: u8,
    pub path: String,
}

impl Entry {
    /// Submodule entries have no blob to read.
    pub fn is_gitlink(&self) -> bool {
        self.mode == "160000"
    }
}

fn git(root: Option<&Path>, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    if let Some(root) = root {
        cmd.current_dir(root);
    }
    let output = cmd.args(args).output().context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// The root of the working tree containing the current directory.
pub fn toplevel() -> anyhow::Result<PathBuf> {
    let out = git(None, &["rev-parse", "--show-toplevel"])?;
    let root = String::from_utf8(out).context("repository path is not UTF-8")?;
    Ok(PathBuf::from(root.trim_end_matches(['\n', '\r'])))
}

/// Every index entry, all stages.
pub fn index_entries(root: &Path) -> anyhow::Result<Vec<Entry>> {
    const BAD: &str = "unexpected `git ls-files --stage` output";
    let out = git(Some(root), &["ls-files", "--stage", "-z"])?;
    out.split(|&b| b == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let record = String::from_utf8_lossy(record);
            let (meta, path) = record.split_once('\t').context(BAD)?;
            let mut meta = meta.split(' ');
            let (Some(mode), Some(oid), Some(stage)) = (meta.next(), meta.next(), meta.next())
            else {
                bail!(BAD);
            };
            Ok(Entry {
                mode: mode.to_owned(),
                oid: oid.to_owned(),
                stage: stage.parse().context(BAD)?,
                path: path.to_owned(),
            })
        })
        .collect()
}

/// Paths whose index version differs from `HEAD` (added, copied, modified,
/// renamed, or type-changed). Without a `HEAD`, every staged path.
pub fn staged_paths(root: &Path) -> anyhow::Result<Vec<String>> {
    let out = git(
        Some(root),
        &[
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--diff-filter=ACMRT",
        ],
    )?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// Contents of the given blobs, in order, through one `git cat-file --batch`.
pub fn read_blobs(root: &Path, oids: &[&str]) -> anyhow::Result<Vec<Vec<u8>>> {
    const BAD: &str = "unexpected `git cat-file --batch` output";
    const READING: &str = "reading `git cat-file --batch` output";
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("cannot run git cat-file")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = oids.join("\n") + "\n";
    // Write from another thread so a full stdout pipe cannot deadlock us.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut blobs = Vec::with_capacity(oids.len());
    for oid in oids {
        let mut header = String::new();
        stdout.read_line(&mut header).context(READING)?;
        let mut fields = header.trim_end().split(' ');
        if fields.next() != Some(oid) {
            bail!(BAD);
        }
        let (Some("blob"), Some(size)) = (fields.next(), fields.next()) else {
            bail!("object {oid} is not a readable blob");
        };
        let size: usize = size.parse().context(BAD)?;
        let mut blob = vec![0; size + 1];
        stdout.read_exact(&mut blob).context(READING)?;
        if blob.pop() != Some(b'\n') {
            bail!(BAD);
        }
        blobs.push(blob);
    }
    writer
        .join()
        .map_err(|_| anyhow!("cat-file writer panicked"))?
        .context("cannot write to git cat-file")?;
    let status = child.wait().context("waiting for git cat-file")?;
    if !status.success() {
        bail!("git cat-file failed: {status}");
    }
    Ok(blobs)
}
