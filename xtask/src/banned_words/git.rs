//! Reading paths and contents from the Git index.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

fn git(root: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new("git");
    if let Some(root) = root {
        cmd.current_dir(root);
    }
    let output = cmd
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// The root of the working tree containing the current directory.
pub fn toplevel() -> Result<PathBuf, String> {
    let out = git(None, &["rev-parse", "--show-toplevel"])?;
    let root = String::from_utf8(out).map_err(|_| "repository path is not UTF-8")?;
    Ok(PathBuf::from(root.trim_end_matches(['\n', '\r'])))
}

/// Every index entry, all stages.
pub fn index_entries(root: &Path) -> Result<Vec<Entry>, String> {
    let out = git(Some(root), &["ls-files", "--stage", "-z"])?;
    out.split(|&b| b == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let record = String::from_utf8_lossy(record);
            let (meta, path) = record
                .split_once('\t')
                .ok_or("unexpected `git ls-files --stage` output")?;
            let mut meta = meta.split(' ');
            let (Some(mode), Some(oid), Some(stage)) = (meta.next(), meta.next(), meta.next())
            else {
                return Err("unexpected `git ls-files --stage` output".to_owned());
            };
            Ok(Entry {
                mode: mode.to_owned(),
                oid: oid.to_owned(),
                stage: stage
                    .parse()
                    .map_err(|_| "unexpected `git ls-files --stage` output")?,
                path: path.to_owned(),
            })
        })
        .collect()
}

/// Paths whose index version differs from `HEAD` (added, copied, modified,
/// renamed, or type-changed). Without a `HEAD`, every staged path.
pub fn staged_paths(root: &Path) -> Result<Vec<String>, String> {
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
pub fn read_blobs(root: &Path, oids: &[&str]) -> Result<Vec<Vec<u8>>, String> {
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
        .map_err(|e| format!("cannot run git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = oids.join("\n") + "\n";
    // Write from another thread so a full stdout pipe cannot deadlock us.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut blobs = Vec::with_capacity(oids.len());
    let bad = || "unexpected `git cat-file --batch` output".to_owned();
    for oid in oids {
        let mut header = String::new();
        stdout.read_line(&mut header).map_err(|e| e.to_string())?;
        let mut fields = header.trim_end().split(' ');
        if fields.next() != Some(oid) {
            return Err(bad());
        }
        let (Some("blob"), Some(size)) = (fields.next(), fields.next()) else {
            return Err(format!("object {oid} is not a readable blob"));
        };
        let size: usize = size.parse().map_err(|_| bad())?;
        let mut blob = vec![0; size + 1];
        stdout.read_exact(&mut blob).map_err(|e| e.to_string())?;
        if blob.pop() != Some(b'\n') {
            return Err(bad());
        }
        blobs.push(blob);
    }
    writer
        .join()
        .map_err(|_| "cat-file writer panicked")?
        .map_err(|e| format!("cannot write to git cat-file: {e}"))?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("git cat-file failed: {status}"));
    }
    Ok(blobs)
}
