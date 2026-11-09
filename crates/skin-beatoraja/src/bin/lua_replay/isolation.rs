//! Reviewed isolation entries (s7-06e-design.md §3.3): a local file, kept
//! with the skins, never in the repository.
//!
//! ```text
//! # a comment
//! root <file> <line> writes [<cell>...]
//! reader <file> <line> <field> replaced|dropped
//! ```
//!
//! A `root` names a stopped root by its definition and its write set
//! (empty allowed); each `reader` after it names a reader of one of its
//! cells and the record its occurrence must carry.

/// One reader and the record its occurrence must carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reader {
    pub file: String,
    pub line: u32,
    pub field: String,
    pub evidence: String,
}

/// One entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub file: String,
    pub line: u32,
    pub writes: Vec<String>,
    pub readers: Vec<Reader>,
}

/// Parses `text`.
///
/// # Errors
///
/// A line that is not a comment, a `root` or a `reader` after a root.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut out: Vec<Entry> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let w: Vec<&str> = line.split_whitespace().collect();
        let bad = || format!("isolation line {}: {line}", n + 1);
        match w.as_slice() {
            ["root", file, l, "writes", cells @ ..] => out.push(Entry {
                file: (*file).to_owned(),
                line: l.parse().map_err(|_| bad())?,
                writes: cells.iter().map(|c| (*c).to_owned()).collect(),
                readers: Vec::new(),
            }),
            ["reader", file, l, field, evidence] => {
                let e = out.last_mut().ok_or_else(bad)?;
                if !matches!(*evidence, "replaced" | "dropped") {
                    return Err(bad());
                }
                e.readers.push(Reader {
                    file: (*file).to_owned(),
                    line: l.parse().map_err(|_| bad())?,
                    field: (*field).to_owned(),
                    evidence: (*evidence).to_owned(),
                });
            }
            _ => return Err(bad()),
        }
    }
    Ok(out)
}

/// Whether chunk name `chunk` (a VM's `@path`) is file `file`: the
/// chunk's path ends with it.
pub fn same_file(chunk: &str, file: &str) -> bool {
    let c = chunk.trim_start_matches('@').replace('\\', "/");
    let f = file.replace('\\', "/");
    c == f || c.ends_with(&format!("/{f}"))
}
