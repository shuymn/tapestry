//! Reviewed isolation entries (s7-06e-design.md §3.3): a local file, kept
//! with the skins, never in the repository.
//!
//! ```text
//! # a comment
//! root <file> <line> writes [<cell>...]
//! reader <cell> <file> <line> <field> replaced <reason words...>
//! reader <cell> <file> <line> <field> dropped
//! reader <cell> none
//! ```
//!
//! A `root` names a stopped root by its definition and its write set
//! (empty for a root that writes nothing); every cell it writes needs at
//! least one `reader` line after it: a reader of that cell with the record
//! its occurrence must carry (a replacement with the reason it must start
//! with, or its destination dropped), or `none`. That the write set and
//! the reader lists are complete is the reviewer's, read from the skin's
//! source: the run checks only what the entry lists.

/// One reader and the record its occurrence must carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reader {
    pub cell: String,
    pub file: String,
    pub line: u32,
    pub field: String,
    /// `replaced` or `dropped`.
    pub evidence: String,
    /// The reason a replacement must start with.
    pub reason: String,
}

/// One entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub file: String,
    pub line: u32,
    pub writes: Vec<String>,
    pub readers: Vec<Reader>,
    /// The cells declared to have no reader.
    pub unread: Vec<String>,
}

/// Parses `text`.
///
/// # Errors
///
/// A line that is not a comment, a `root` or a `reader` after a root, or
/// a reader of a cell its root does not write.
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
                unread: Vec::new(),
            }),
            ["reader", cell, "none"] => {
                let e = out.last_mut().ok_or_else(bad)?;
                if !e.writes.iter().any(|w| w == cell) {
                    return Err(bad());
                }
                e.unread.push((*cell).to_owned());
            }
            ["reader", cell, file, l, field, evidence, reason @ ..] => {
                let e = out.last_mut().ok_or_else(bad)?;
                if !e.writes.iter().any(|w| w == cell)
                    || !matches!(*evidence, "replaced" | "dropped")
                    || (*evidence == "replaced") == reason.is_empty()
                {
                    return Err(bad());
                }
                e.readers.push(Reader {
                    cell: (*cell).to_owned(),
                    file: (*file).to_owned(),
                    line: l.parse().map_err(|_| bad())?,
                    field: (*field).to_owned(),
                    evidence: (*evidence).to_owned(),
                    reason: reason.join(" "),
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
